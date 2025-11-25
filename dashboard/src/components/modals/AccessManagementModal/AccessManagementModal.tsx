import React, { useState, useEffect } from "react";
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
import { AlertBox } from "../../ui/alert-box";
import {
  useGroups,
  useAddModelToGroup,
  useRemoveModelFromGroup,
  type Model,
  type Group as BackendGroup,
} from "../../../api/control-layer";

interface AccessManagementModalProps {
  isOpen: boolean;
  onClose: () => void;
  model: Model;
}

export const AccessManagementModal: React.FC<AccessManagementModalProps> = ({
  isOpen,
  onClose,
  model,
}) => {
  const [error, setError] = useState<string | null>(null);

  // Fetch all groups data (includes models for access checking)
  // Only fetch when modal is open to avoid 403 errors for users without permission
  const { data: groupsData, isLoading: loading } = useGroups({
    include: "models",
    enabled: isOpen,
  });
  const addModelToGroupMutation = useAddModelToGroup();
  const removeModelFromGroupMutation = useRemoveModelFromGroup();

  // Clear error when modal opens
  useEffect(() => {
    if (isOpen) {
      setError(null);
    }
  }, [isOpen, model]);

  // Check if group has access to the model using the groups data
  const groupHasAccessToModel = (groupId: string): boolean => {
    const group = groupsData?.data.find((g) => g.id === groupId);
    const result = group?.models?.some((m) => m.id === model.id) || false;
    return result;
  };

  // Helper function to check if a specific group is being updated
  const isGroupUpdating = (groupId: string) => {
    return (
      (addModelToGroupMutation.isPending &&
        addModelToGroupMutation.variables?.groupId === groupId &&
        addModelToGroupMutation.variables?.modelId === model.id) ||
      (removeModelFromGroupMutation.isPending &&
        removeModelFromGroupMutation.variables?.groupId === groupId &&
        removeModelFromGroupMutation.variables?.modelId === model.id)
    );
  };

  const handleGrantAccess = async (groupId: string) => {
    setError(null);
    try {
      await addModelToGroupMutation.mutateAsync({ groupId, modelId: model.id });
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to grant group access",
      );
    }
  };

  const handleRevokeAccess = async (groupId: string) => {
    setError(null);
    try {
      await removeModelFromGroupMutation.mutateAsync({
        groupId,
        modelId: model.id,
      });
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to revoke group access",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[80vh] overflow-hidden">
        <DialogHeader>
          <DialogTitle>Manage Access</DialogTitle>
          <DialogDescription>Model: {model.model_name}</DialogDescription>
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
                  Groups
                </h3>
                <p className="text-sm text-gray-600">
                  Manage which groups have access to this model. Users in these
                  groups will be able to use this model.
                </p>
              </div>

              {!groupsData || groupsData.data.length === 0 ? (
                <div className="text-center py-8">
                  <Users className="w-12 h-12 text-gray-400 mx-auto mb-3" />
                  <p className="text-gray-500">No groups available</p>
                  <p className="text-sm text-gray-400 mb-4">
                    Create groups first to manage access
                  </p>
                  <button
                    onClick={() => {
                      // Close this modal and navigate to users-groups page to create groups
                      onClose();
                      window.location.hash = "#/users-groups";
                    }}
                    className="px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 transition-colors"
                  >
                    Create Groups
                  </button>
                </div>
              ) : (
                groupsData.data.map((group: BackendGroup) => (
                  <div
                    key={group.id}
                    className="flex items-center justify-between p-4 border border-gray-200 rounded-lg hover:bg-gray-50 transition-colors"
                  >
                    <div className="flex items-center gap-3 flex-1 min-w-0 pr-10">
                      <div className="w-10 h-10 bg-blue-500 rounded-lg flex items-center justify-center shrink-0">
                        <Users className="w-5 h-5 text-white" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <h4 className="font-medium text-gray-900">
                          {group.name}
                        </h4>
                        {group.description && (
                          <p
                            className="text-sm text-gray-500 truncate cursor-help"
                            title={group.description}
                          >
                            {group.description}
                          </p>
                        )}
                      </div>
                    </div>

                    <div className="flex items-center gap-2 shrink-0">
                      {groupHasAccessToModel(group.id) ? (
                        <>
                          <span className="text-xs px-2 py-1 bg-green-100 text-green-700 rounded-full">
                            Has Access
                          </span>
                          <button
                            onClick={() => handleRevokeAccess(group.id)}
                            disabled={isGroupUpdating(group.id)}
                            className="p-2 text-red-600 hover:bg-red-50 rounded-lg transition-colors disabled:opacity-50"
                            title="Revoke access"
                          >
                            {isGroupUpdating(group.id) ? (
                              <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-red-600"></div>
                            ) : (
                              <Trash2 className="w-4 h-4" />
                            )}
                          </button>
                        </>
                      ) : (
                        <button
                          onClick={() => handleGrantAccess(group.id)}
                          disabled={isGroupUpdating(group.id)}
                          className="flex items-center gap-2 px-3 py-2 text-blue-600 hover:bg-blue-50 rounded-lg transition-colors disabled:opacity-50"
                        >
                          {isGroupUpdating(group.id) ? (
                            <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-blue-600"></div>
                          ) : (
                            <Plus className="w-4 h-4" />
                          )}
                          <span className="text-sm">Grant Access</span>
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
