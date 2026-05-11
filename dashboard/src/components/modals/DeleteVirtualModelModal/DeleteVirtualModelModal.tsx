import React, { useState } from "react";
import { AlertTriangle, Layers } from "lucide-react";
import { useDeleteModel } from "../../../api/control-layer";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { AlertBox } from "../../ui/alert-box";

interface DeleteVirtualModelModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  modelId: string;
  modelAlias: string;
  modelName: string;
  /** Number of hosted models composing this virtual model. Surfaced so the
   *  user knows whether they're about to drop something cascading. */
  componentCount: number;
}

export const DeleteVirtualModelModal: React.FC<DeleteVirtualModelModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  modelId,
  modelAlias,
  modelName,
  componentCount,
}) => {
  const [error, setError] = useState<string | null>(null);
  const deleteModelMutation = useDeleteModel();

  const handleDelete = async () => {
    setError(null);
    try {
      await deleteModelMutation.mutateAsync(modelId);
      onSuccess();
      onClose();
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to delete virtual model",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <AlertTriangle className="w-5 h-5 text-red-600" />
            </div>
            <div>
              <DialogTitle>Delete virtual model</DialogTitle>
              <DialogDescription>
                This action cannot be undone
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div className="space-y-4">
          {error && (
            <AlertBox variant="error" className="mb-4">
              {error}
            </AlertBox>
          )}

          <p className="text-gray-700">
            Are you sure you want to delete the virtual model{" "}
            <strong>{modelAlias}</strong>?
          </p>

          <div
            className="bg-gray-50 rounded-lg p-3"
            role="group"
            aria-labelledby="virtual-model-details-heading"
          >
            <h4 id="virtual-model-details-heading" className="sr-only">
              Virtual Model Details
            </h4>
            <p className="text-sm text-gray-600 flex items-center gap-1.5">
              <Layers className="w-3.5 h-3.5 text-violet-600" />
              <strong>Alias:</strong> {modelAlias}
            </p>
            <p className="text-sm text-gray-600 mt-1">
              <strong>Name:</strong> {modelName}
            </p>
            <p className="text-sm text-gray-600 mt-1">
              <strong>Hosted models:</strong>{" "}
              {componentCount === 0
                ? "none"
                : `${componentCount} component${componentCount === 1 ? "" : "s"} (will be detached, not deleted)`}
            </p>
          </div>

          <div
            className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg"
            role="alert"
            aria-label="Deletion warning"
          >
            <p className="text-sm text-yellow-800">
              <strong>Warning:</strong> Requests routed to{" "}
              <code className="font-mono text-[12px]">{modelAlias}</code> will
              fail after deletion. The underlying hosted models stay intact —
              only the virtual model itself is removed. Any traffic routing
              rules pointing here will be invalidated.
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            onClick={onClose}
            disabled={deleteModelMutation.isPending}
            variant="outline"
            aria-label="Cancel deletion"
          >
            Cancel
          </Button>
          <Button
            onClick={handleDelete}
            disabled={deleteModelMutation.isPending}
            variant="destructive"
            className="gap-2"
            aria-label={
              deleteModelMutation.isPending
                ? "Deleting virtual model"
                : "Confirm deletion"
            }
          >
            {deleteModelMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Deleting...
              </>
            ) : (
              <>
                <AlertTriangle className="w-4 h-4" />
                Delete virtual model
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
