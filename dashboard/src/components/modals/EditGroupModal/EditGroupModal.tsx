import React, { useState, useEffect } from "react";
import { Users } from "lucide-react";
import { useUpdateGroup } from "../../../api/clay";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";

interface EditGroupModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  groupId: string;
  currentGroup: {
    name: string;
    description: string;
  };
}

export const EditGroupModal: React.FC<EditGroupModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  groupId,
  currentGroup,
}) => {
  const [formData, setFormData] = useState({
    name: currentGroup.name,
    description: currentGroup.description,
  });
  const [error, setError] = useState<string | null>(null);

  const updateGroupMutation = useUpdateGroup();

  // Reset form when modal opens
  useEffect(() => {
    if (isOpen) {
      setFormData({
        name: currentGroup.name,
        description: currentGroup.description,
      });
      setError(null);
    }
  }, [isOpen, currentGroup]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();

    if (!formData.name.trim()) {
      setError("Group name is required");
      return;
    }

    setError(null);

    try {
      await updateGroupMutation.mutateAsync({
        id: groupId,
        data: {
          name: formData.name.trim(),
          description: formData.description.trim() || undefined,
        },
      });

      onSuccess();
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update group");
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3 mb-2">
            <div className="w-10 h-10 bg-blue-100 rounded-full flex items-center justify-center">
              <Users className="w-5 h-5 text-blue-600" />
            </div>
            <div>
              <DialogTitle>Edit Group</DialogTitle>
              <DialogDescription>{currentGroup.name}</DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <form
          id="edit-group-form"
          onSubmit={handleSubmit}
          className="space-y-4"
        >
          {error && (
            <div className="mb-4 p-3 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-sm text-red-600">{error}</p>
            </div>
          )}

          <div className="space-y-4">
            <div>
              <label
                htmlFor="name"
                className="block text-sm font-medium text-gray-700 mb-1"
              >
                Group Name <span className="text-red-500">*</span>
              </label>
              <input
                type="text"
                id="name"
                value={formData.name}
                onChange={(e) =>
                  setFormData((prev) => ({ ...prev, name: e.target.value }))
                }
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
                placeholder="Enter group name"
                required
              />
            </div>

            <div>
              <label
                htmlFor="description"
                className="block text-sm font-medium text-gray-700 mb-1"
              >
                Description
              </label>
              <textarea
                id="description"
                value={formData.description}
                onChange={(e) =>
                  setFormData((prev) => ({
                    ...prev,
                    description: e.target.value,
                  }))
                }
                rows={3}
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent resize-none"
                placeholder="Enter group description (optional)"
              />
            </div>

            <div className="bg-gray-50 rounded-lg p-3">
              <p className="text-sm text-gray-600">
                <strong>Group ID:</strong> {groupId}
              </p>
              <p className="text-sm text-gray-600 mt-1">
                <strong>Note:</strong> Changing the group name will update it
                everywhere it's used.
              </p>
            </div>
          </div>
        </form>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={onClose}
            disabled={updateGroupMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            form="edit-group-form"
            disabled={updateGroupMutation.isPending}
          >
            {updateGroupMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Saving...
              </>
            ) : (
              "Save Changes"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
